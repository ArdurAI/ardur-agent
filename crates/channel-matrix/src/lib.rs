//! ardur-channel-matrix — the second real channel backend behind the §4.0
//! [`MessagingGateway`] contract (alongside [`ardur-slack-adapter`]).
//!
//! Plan family: §4.X (Matrix channel adapter Phase 1).
//!
//! Matrix is an open, federated, Rust-native protocol — a natural second channel
//! for self-hosted ardur deployments. This crate wraps the official
//! [`matrix_sdk`] client (`matrix-org/matrix-rust-sdk`).
//!
//! # Phase 1 (this crate)
//!
//! - [`MatrixChannel`] — implements [`MessagingGateway`]. Construct with
//!   [`MatrixChannel::new`] (async: builds the client, opens the sqlite
//!   state/crypto store, restores the bot session), then call
//!   [`start_sync`](MatrixChannel::start_sync) once to begin draining inbound
//!   traffic.
//! - **Outbound**: [`MessagingGateway::send_message`] posts plaintext via
//!   `room.send` and returns a [`MessageReceipt`] whose `provider_message_id` is
//!   the homeserver event id. [`MatrixChannel::send_text`] is the native send
//!   that preserves the full [`MatrixError`] taxonomy.
//! - **Inbound**: unlike the webhook-push Slack adapter,
//!   [`MessagingGateway::receive`] is a real long-poll here — the sync task's
//!   event handler forwards each room text event onto an internal queue, gating
//!   on the room allowlist (deny-by-default when empty) and dropping the bot's
//!   own messages (echo prevention). Invites are auto-accepted only when
//!   [`MatrixConfig::auto_join_invites`] is `true` (default `false`, ARD-422)
//!   and the room clears the allowlist.
//! - [`MatrixConfig`] — built via [`MatrixConfig::builder`] or
//!   [`MatrixConfig::from_env`] (`MATRIX_HOMESERVER_URL`, `MATRIX_USER_ID`,
//!   `MATRIX_ACCESS_TOKEN`, `MATRIX_DEVICE_ID`, `MATRIX_STATE_DIR`,
//!   `MATRIX_AUTO_JOIN_INVITES`, `MATRIX_ALLOWED_ROOMS`).
//! - [`MatrixError`] — the typed failure surface;
//!   [`into_gateway_error`](MatrixError::into_gateway_error) lowers it onto
//!   [`GatewayError`] at the trait boundary.
//!
//! # Adapt-points vs. the §4.X task brief
//!
//! - The brief named `matrix-sdk` 0.8 with `sled` + `rustls-tls` features; the
//!   current release is 0.18, which dropped the `sled` store (now `sqlite`) and
//!   the TLS feature flags (rustls is the built-in reqwest backend). The crate
//!   pins 0.18 with `e2e-encryption` + `bundled-sqlite`.
//! - The brief named a `MatrixConfig::state_dir` for a sled store; it backs the
//!   sqlite state + crypto store instead.
//!
//! # E2EE caveat
//!
//! With `e2e-encryption` on, decryption "just works" once the bot's device keys
//! are in the crypto store. For production, run the bot once with a stable
//! [`device_id`](MatrixConfig::device_id) so the store persists, and verify the
//! device from a trusted session on first run — otherwise messages in encrypted
//! rooms may arrive undecryptable until keys are shared. See this crate's README.
//!
//! [`ardur-slack-adapter`]: https://docs.rs/ardur-slack-adapter
//! [`MessagingGateway`]: ardur_messaging_gateway::MessagingGateway
//! [`MessageReceipt`]: ardur_messaging_gateway::MessageReceipt
//! [`GatewayError`]: ardur_messaging_gateway::GatewayError
//! [`MessagingGateway::send_message`]: ardur_messaging_gateway::MessagingGateway::send_message
//! [`MessagingGateway::receive`]: ardur_messaging_gateway::MessagingGateway::receive
#![forbid(unsafe_code)]
#![warn(missing_docs)]
// The matrix-sdk event-handler machinery builds deeply nested `Send` obligations
// (decryption-error result types behind the async sync loop); the default
// recursion limit overflows evaluating them. Raise it so auto-trait solving
// converges. (Tracked upstream; a known requirement for SDK event handlers.)
#![recursion_limit = "256"]

mod channel;
mod config;
mod error;

pub use channel::MatrixChannel;
pub use config::{
    DEFAULT_DEVICE_ID, MatrixConfig, MatrixConfigBuilder, default_state_dir, parse_allowed_rooms,
};
pub use error::MatrixError;

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    use config::{
        ENV_ACCESS_TOKEN, ENV_ALLOWED_ROOMS, ENV_AUTO_JOIN, ENV_DEVICE_ID, ENV_HOMESERVER,
        ENV_STATE_DIR, ENV_USER_ID,
    };

    /// A fake variable source backing `MatrixConfig::from_source` — so the
    /// `from_env` logic is unit-tested without mutating process-global env (which
    /// is `unsafe` under edition 2024, and this crate is `#![forbid(unsafe_code)]`).
    struct FakeEnv(HashMap<&'static str, &'static str>);

    impl FakeEnv {
        /// Start from the three required Matrix credentials.
        fn required() -> Self {
            Self(HashMap::from([
                (ENV_HOMESERVER, "https://matrix.example.org"),
                (ENV_USER_ID, "@ardur-bot:example.org"),
                (ENV_ACCESS_TOKEN, "syt_secret_token"),
            ]))
        }

        fn with(mut self, key: &'static str, value: &'static str) -> Self {
            self.0.insert(key, value);
            self
        }

        /// Resolve `MatrixConfig` against this source.
        fn resolve(&self) -> Result<MatrixConfig, MatrixError> {
            MatrixConfig::from_source(|k| self.0.get(k).map(|v| (*v).to_owned()))
        }
    }

    #[test]
    fn config_from_env_defaults() {
        let config = FakeEnv::required().resolve().expect("required vars set");
        assert_eq!(config.homeserver_url, "https://matrix.example.org");
        assert_eq!(config.user_id, "@ardur-bot:example.org");
        // Optional knobs fall back to their documented defaults.
        assert_eq!(config.device_id, None);
        assert_eq!(config.resolved_device_id(), DEFAULT_DEVICE_ID);
        assert!(
            !config.auto_join_invites,
            "auto-join defaults to false when unset (ARD-422)"
        );
        assert!(
            config.allowed_rooms.is_empty(),
            "an unset allowlist means all rooms denied (deny-by-default)"
        );
        assert!(
            !config.room_allowed("!any:hs"),
            "empty allowlist denies all rooms"
        );
        assert!(
            config.state_dir.ends_with("matrix-state"),
            "state dir defaults under ~/.ardur, got {:?}",
            config.state_dir
        );
    }

    #[test]
    fn config_from_env_reads_overrides() {
        let config = FakeEnv::required()
            .with(ENV_DEVICE_ID, "ARDUR_PROD_01")
            .with(ENV_STATE_DIR, "/tmp/ardur-matrix")
            .with(ENV_AUTO_JOIN, "false")
            .with(ENV_ALLOWED_ROOMS, "!room1:example.org,!room2:example.org")
            .resolve()
            .expect("required vars set");
        assert_eq!(config.device_id.as_deref(), Some("ARDUR_PROD_01"));
        assert_eq!(config.resolved_device_id(), "ARDUR_PROD_01");
        assert_eq!(
            config.state_dir,
            std::path::PathBuf::from("/tmp/ardur-matrix")
        );
        assert!(
            !config.auto_join_invites,
            "explicit false disables auto-join"
        );
        assert_eq!(
            config.allowed_rooms,
            vec![
                "!room1:example.org".to_string(),
                "!room2:example.org".to_string()
            ]
        );
        // A gated room is permitted; an unlisted one is not.
        assert!(config.room_allowed("!room1:example.org"));
        assert!(!config.room_allowed("!other:example.org"));
    }

    #[test]
    fn config_from_env_requires_homeserver_when_no_env() {
        // An empty source: the first missing required var is the homeserver.
        let err =
            MatrixConfig::from_source(|_| None).expect_err("missing required vars must error");
        assert!(
            matches!(&err, MatrixError::MissingEnvVar(v) if v == ENV_HOMESERVER),
            "the first missing required var is the homeserver, got: {err}"
        );
    }

    #[test]
    fn room_allowlist_parsing() {
        // Comma-split with surrounding whitespace trimmed and blanks dropped.
        let parsed = parse_allowed_rooms(Some(" !a:hs ,!b:hs,  ,!c:hs "));
        assert_eq!(
            parsed,
            vec![
                "!a:hs".to_string(),
                "!b:hs".to_string(),
                "!c:hs".to_string()
            ]
        );
        // None (and an all-blank value) parse to an empty list = "all denied".
        assert!(parse_allowed_rooms(None).is_empty());
        assert!(parse_allowed_rooms(Some("   ,  , ")).is_empty());
    }

    #[test]
    fn builder_requires_non_empty_fields() {
        // The builder rejects an empty required field with a named error.
        let err = MatrixConfig::builder("", "@bot:hs", "token")
            .build()
            .expect_err("empty homeserver is rejected");
        assert!(matches!(err, MatrixError::MissingField(f) if f == "homeserver_url"));

        // A fully-specified builder produces the configured values.
        let config = MatrixConfig::builder("https://hs", "@bot:hs", "token")
            .device_id("DEV1")
            .auto_join_invites(false)
            .allowed_rooms(vec!["!r:hs".to_string()])
            .build()
            .expect("all required fields present");
        assert_eq!(config.homeserver_url, "https://hs");
        assert_eq!(config.resolved_device_id(), "DEV1");
        assert!(!config.auto_join_invites);
        assert!(config.room_allowed("!r:hs"));
        assert!(!config.room_allowed("!nope:hs"));
    }
}
