//! ardur-admin — an observability + operator-console dashboard over an
//! ardur-server deployment's persisted artifacts (§13.X).
//!
//! ardur-server writes three things to its data directory that are useful to an
//! operator after the fact: per-session [journals](ardur_session_journals)
//! (`<data>/journals/sessions/<id>/journal.jsonl`), a hash-chained
//! [receipt](ardur_receipt) log (`<data>/receipts/chain.jsonl`), and — when the
//! durable memory backend is selected — a Qdrant collection. This crate reads
//! those artifacts *directly* and serves them over a small HTTP dashboard.
//!
//! It is deliberately decoupled from ardur-server: a separate binary on its own
//! port, configured purely by CLI flags (no env, no shared `Config`). Every
//! filesystem and Qdrant access is a read, with one narrow, explicit
//! exception: the [`approvals`] module proxies approve/reject decisions to
//! ardur-server's own admin-bearer-gated `/approvals` API when
//! `--server-url`/`--server-admin-token` are configured — admin-ui itself
//! never opens the approvals store, or any other artifact, for write. See
//! the crate `README.md` for the full security model.
//!
//! The HTTP surface is built by [`build_router`]; the integration tests drive it
//! in-process with `axum_test::TestServer`.
#![forbid(unsafe_code)]

pub mod approvals;
pub mod auth;
pub mod config;
pub mod costs;
pub mod html;
pub mod journal;
pub mod memory;
pub mod receipts;
pub mod routes;
pub mod state;
pub mod trust;

pub use config::Cli;
pub use routes::build_router;
pub use state::AppState;
