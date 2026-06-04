//! ardur-server — the first deployable Ardur binary, as a library.
//!
//! The binary (`src/bin/ardur-server.rs`) is a thin shell: it installs tracing,
//! reads [`Config`] from the environment, builds the live Anthropic provider,
//! boots [`AppState`], and serves [`build_router`] over a TCP listener with
//! graceful shutdown. Everything testable lives here so the integration tests in
//! `tests/` can boot an [`AppState`] over a stub provider + tempdir and drive
//! the router through `tower::ServiceExt::oneshot` — no socket, no live Slack.
//!
//! # What the server does
//!
//! It fronts the fused Phase-2 runtime with a Slack Events-API webhook. Each
//! inbound user message is verified (HMAC + replay), authorized by a freshly
//! minted short-lived cap-token, run through the ten-stage
//! [`FusedRuntime`](ardur_fused_runtime::FusedRuntime) pipeline (cap-token →
//! cedar → cost-gate → provider → receipt → journal → memory), and the reply is
//! posted back to the channel. See [`AppState`] for the boot sequence and the
//! Phase-3 markers (in-process memory; per-process rather than per-Slack-user
//! budget).
#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod config;
mod mcp;
mod routes;
mod state;

pub use config::{Config, LogFormat, MissingEnvVar};
pub use mcp::{build_mcp_router, example_registry};
pub use routes::build_router;
pub use state::{AUDIENCE, AppState, CAP_TTL_SECS, GATEWAY_SUBJECT, McpSurface, TOOL};
