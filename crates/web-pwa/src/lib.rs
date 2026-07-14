//! ardur-web-pwa — installable PWA support: a VAPID (RFC 8292) identity key,
//! a Web Push subscription store, and Web Push send, mounted by the server
//! under an opt-in HTTP surface alongside the static `web-client/` shell.
//!
//! Plan family: §10.14 (`plans/10.14-pwa-installable-web-client-blueprint.md`
//! — "PWA Install, Service Worker, Web Push Subscribe, And Test Blueprint").
//! `web-client/manifest.webmanifest` + `web-client/sw.js` already exist (the
//! static app-shell install path); this crate supplies the piece the
//! blueprint says is missing — "No `crates/web-pwa/` exists... no VAPID key
//! pair is generated" — the server-side VAPID/subscribe/send substrate the
//! client's `sw.js`/`app.js` already know how to call into.
//!
//! # Phase 1 (this crate)
//!
//! - [`VapidKeyPair`] — generate-once, persist-to-disk P-256 identity key
//!   ([`keys`]).
//! - [`PushSubscription`] / [`SubscriptionStore`] — validated (HTTPS-only,
//!   ≤2048 char endpoint) subscription registration, persisted as JSON
//!   ([`subscription`]).
//! - [`push::send`] — VAPID-sign + RFC 8188 `aes128gcm`-encrypt + deliver one
//!   notification ([`push`]).
//! - [`PwaService`] + [`router`] — the `axum::Router` the server mounts:
//!   `GET /vapid-public-key`, `POST /subscribe`, `POST /unsubscribe`,
//!   `POST /test`.
//!
//! # Adapt-points vs. the §10.14 task brief
//!
//! The full blueprint additionally calls for: a Cedar policy gate evaluated
//! before every send (`pwa.push.denied.v1` on refusal); JWS-ES256 + TSA-
//! countersigned receipts on every subscribe/unsubscribe/send/deliver/fail
//! transition; subscriptions persisted as §7.3 SessionStore-projected rows
//! rather than a flat JSON file; per-tenant VAPID key + subscription
//! partitioning; a per-principal rate limit (30/hour default); and a
//! `push.web.send` endpoint for arbitrary sender-initiated notifications (this
//! crate exposes only [`PwaService::send_test`] over HTTP — a deliberately
//! narrow, self-targeted "does my subscription work" probe — plus
//! [`PwaService::send`] as a Rust API for other in-process callers; an open,
//! unauthenticated "send to any subscription" HTTP route is not part of this
//! Phase-1 surface). All of the above are Phase 2 — this PR ships the
//! substrate the rest of §10.14 builds on, matching the same "P0 scaffold now,
//! richer policy later" precedent the §4.4/§4.5 channel-adapter catalog set
//! for the channel adapters.
//!
//! [`web_push`]: https://docs.rs/web-push

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod error;
mod keys;
mod push;
mod routes;
mod subscription;

pub use error::PwaError;
pub use keys::VapidKeyPair;
pub use push::PushPayload;
pub use routes::{PwaService, router};
pub use subscription::{PushSubscription, SubscriptionStore};
