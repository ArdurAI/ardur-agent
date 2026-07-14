//! ardur-channel-email — an email backend behind the §4.0
//! [`MessagingGateway`] contract (alongside [`ardur-slack-adapter`],
//! [`ardur-channel-matrix`], [`ardur-channel-discord`], and
//! [`ardur-channel-telegram`]).
//!
//! Plan family: §4.4 (Channel Adapter Catalog, Hermes-derived) — the Cycle-4
//! P0 subset (`plans/4.4-channel-adapter-catalog-hermes-blueprint.md` lines
//! 82-83, 1503-1528).
//!
//! # Phase 1 (this crate)
//!
//! - [`EmailChannel`] — implements [`MessagingGateway`]. Construct with
//!   [`EmailChannel::new`] (validates the IMAP credentials via a login/logout
//!   round trip and builds the SMTP transport), then call
//!   [`start`](EmailChannel::start) once to begin the inbox poll loop.
//! - **Outbound**: [`MessagingGateway::send_message`] sends plaintext via
//!   SMTP (STARTTLS submission) and returns a [`MessageReceipt`].
//! - **Inbound**: a blocking poll loop (see the adapt-points note below)
//!   searches `INBOX` for `UNSEEN` messages, parses each with
//!   [`mail_parser`], gates on the sender allowlist, forwards the plaintext
//!   body onto an internal queue, and marks the message `\Seen`.
//!   [`MessagingGateway::receive`] pops the next one.
//! - [`EmailConfig`] — built via [`EmailConfig::builder`] or
//!   [`EmailConfig::from_env`] (`ARDUR_EMAIL_ADDRESS`, `ARDUR_EMAIL_PASSWORD`,
//!   `ARDUR_EMAIL_IMAP_HOST`, `ARDUR_EMAIL_IMAP_PORT`,
//!   `ARDUR_EMAIL_SMTP_HOST`, `ARDUR_EMAIL_SMTP_PORT`,
//!   `ARDUR_EMAIL_ALLOWED_SENDERS`, `ARDUR_EMAIL_POLL_INTERVAL_SECS`).
//! - [`EmailError`] — the typed failure surface;
//!   [`into_gateway_error`](EmailError::into_gateway_error) lowers it onto
//!   [`GatewayError`] at the trait boundary.
//!
//! # Adapt-points vs. the §4.4 task brief
//!
//! - The brief names "IMAP IDLE for receive"; this crate polls instead (a
//!   blocking loop: search `UNSEEN` → fetch → sleep → repeat). `IDLE` needs a
//!   persistent connection with its own reconnect/keepalive state machine and
//!   is not exposed as an `async` primitive by the `imap` crate — Phase 2
//!   work, tracked as a `// TODO §4.4 Phase 2` in `channel.rs`.
//! - The brief names "DKIM verification"; Phase 1 has no DKIM/SPF/DMARC
//!   verification — inbound trust is enforced purely by the sender
//!   allowlist (deny-by-default, per ARD-475, matching the other adapters).
//!   Adding DKIM verification is Phase 2.
//! - The brief names "multipart HTML+plain"; Phase 1 extracts the first
//!   `text/plain` part only (via `mail_parser::Message::body_text`).
//!   HTML-only bodies and attachments are Phase 2 — an HTML-only inbound
//!   message currently forwards as an empty body rather than erroring, and
//!   [`MessageBody::Attachment`] is rejected on send.
//! - The `imap` crate is at `3.0.0-alpha.15` — the only maintained major
//!   version of `rust-imap`, matching the version-bump precedent the
//!   Discord/Telegram/Matrix adapters already set (pinning past a brief's
//!   guessed version to the actual latest release).
//!
//! [`ardur-slack-adapter`]: https://docs.rs/ardur-slack-adapter
//! [`ardur-channel-matrix`]: https://docs.rs/ardur-channel-matrix
//! [`ardur-channel-discord`]: https://docs.rs/ardur-channel-discord
//! [`ardur-channel-telegram`]: https://docs.rs/ardur-channel-telegram
//! [`MessagingGateway`]: ardur_messaging_gateway::MessagingGateway
//! [`MessageReceipt`]: ardur_messaging_gateway::MessageReceipt
//! [`GatewayError`]: ardur_messaging_gateway::GatewayError
//! [`MessagingGateway::send_message`]: ardur_messaging_gateway::MessagingGateway::send_message
//! [`MessagingGateway::receive`]: ardur_messaging_gateway::MessagingGateway::receive
//! [`MessageBody::Attachment`]: ardur_messaging_gateway::MessageBody::Attachment
#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod channel;
mod config;
mod error;

pub use channel::EmailChannel;
pub use config::{EmailConfig, EmailConfigBuilder, parse_allowed_senders};
pub use error::EmailError;
