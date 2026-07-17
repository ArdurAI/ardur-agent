//! `ardur-cron-ui` — the operator-facing cron management surface (§9.4).
//!
//! A stateless inspector/controller over the durable cron scheduler state and
//! its receipts. It is the first operator-facing UI surface in the cron family:
//! list scheduled crons with filters, inspect per-cron run history, and
//! create / pause / resume / edit / delete from the command surface — all
//! cap-token-scoped, read-only by default, receipt-audited, and secret-redacted
//! on render.
//!
//! # Shape
//!
//! - [`CronController`](controller::CronController) — the stateless controller.
//!   `list` / `detail` read; `mutate` writes. Every call emits a receipt via a
//!   [`ReceiptSink`](gate::ReceiptSink); mutations are refused unless the
//!   operator's cap-token carries [`SCOPE_MUTATE`](gate::SCOPE_MUTATE).
//! - [`CronStore`](store::CronStore) — durable cron persistence
//!   ([`FileCronStore`](store::FileCronStore) / [`InMemoryCronStore`](store::InMemoryCronStore)).
//! - [`CapGate`](gate::CapGate) — verifies operator cap-tokens against the
//!   issuer root and returns a [`Principal`](gate::Principal) with the
//!   effective scope set.
//! - [`Redactor`](redaction::Redactor) — rule-based sentinel scan applied to
//!   every rendered field.
//! - [`CronFilter`](filter::CronFilter) — typed, AND-composed filter chips.
//!
//! # Security posture
//!
//! - **Read-only by default** (ADR-Phase3-278): mutations need `cron.ui.mutate`.
//! - **Cap-token-scoped visibility** (ADR-Phase3-279): `SelfOnly` by default;
//!   `Project`/`Tenant` need `cron.ui.project` / `cron.ui.admin`.
//! - **Sentinel scan on render** (ADR-Phase3-280): a credential planted in a
//!   cron field never displays verbatim.
//! - **Receipt on every action**: view + mutate both receipt; every mutate
//!   attempt receipts regardless of success.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod controller;
pub mod domain;
pub mod error;
pub mod filter;
pub mod gate;
pub mod redaction;
pub mod render;
pub mod store;

pub use controller::{CronController, validate_cron};
pub use domain::{
    CreateRequest, CronDetail, CronMutation, CronRecord, CronRow, CronStatus, DeliveryMode,
    Density, EditChanges, MutationReport, RunStatus, RunSummary, UiMode, VisibilityTier,
};
pub use error::{CronUiError, Result};
pub use filter::{CronFilter, StatusSet};
pub use gate::{
    CapGate, Es256ReceiptSink, InMemoryReceiptSink, Principal, ReceiptEvent, ReceiptSink,
    RecordedReceipt, SCOPE_ADMIN, SCOPE_MUTATE, SCOPE_PROJECT, SCOPE_VIEW, fingerprint,
};
pub use redaction::{REDACTED, Redactor};
pub use render::{render_detail, render_list};
pub use store::{CronStore, FileCronStore, InMemoryCronStore, MAX_RUN_HISTORY};
